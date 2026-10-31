//! 可信宿主装配的离线 Cargo 编译器；不执行生成产物，不赋予任何宿主工具权限。
use crate::{
    CargoCompilationFailure, CargoCompilerConfig, CargoProject, Result,
    cargo_model::valid_digest,
    compiler::COMPILE_SLOTS,
    fail, reject,
    runtime::{check_stop, guarded, reserve},
};
use eve_llm_api::{ContextScope, ToolExecutionContext};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

const ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
const CONTROL: &str = include_str!("cargo_controller.py");
pub struct CargoCompiler {
    config: CargoCompilerConfig,
}
impl std::fmt::Debug for CargoCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CargoCompiler")
            .field("bundle_sha256", &self.config.bundle_sha256)
            .finish_non_exhaustive()
    }
}
pub enum CargoCompilation {
    Ready(Arc<CargoArtifact>),
    Failed(CargoCompilationFailure),
}
pub struct CargoArtifact {
    _directory: tempfile::TempDir,
    binary: PathBuf,
    sha256: String,
    bundle_sha256: String,
    owner: ContextScope,
}
impl CargoArtifact {
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn bundle_sha256(&self) -> &str {
        &self.bundle_sha256
    }
    pub fn owner(&self) -> &ContextScope {
        &self.owner
    }
    /// 只能交给已授权沙箱；这个句柄不授予在宿主直接执行的权限。
    pub fn verified_binary(&self) -> Result<&Path> {
        let (hash, _) = hash_file(&self.binary, ARTIFACT_BYTES)?;
        if hash != self.sha256 {
            return Err(reject("Cargo产物校验失败，未自动重新编译"));
        }
        Ok(&self.binary)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    ok: bool,
    code: String,
    stage: Option<String>,
    reason: Option<String>,
    exit_code: Option<i32>,
    #[serde(default)]
    diagnostics: String,
    #[serde(default)]
    truncated: bool,
    artifact_bytes: Option<usize>,
    artifact_sha256: Option<String>,
    limits: Option<serde_json::Value>,
}
fn hash_file(path: &Path, limit: usize) -> Result<(String, usize)> {
    let metadata = fs::symlink_metadata(path).map_err(|_| fail("Cargo资源不存在"))?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(reject("Cargo资源类型或大小无效"));
    }
    let mut source = fs::File::open(path)
        .map_err(|_| fail("Cargo资源不可读"))?
        .take((limit + 1) as u64);
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut size = 0;
    let mut buffer = [0; 65536];
    loop {
        let count = match source.read(&mut buffer) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(fail("Cargo资源读取失败")),
        };
        if count == 0 {
            break;
        }
        size += count;
        if size > limit {
            return Err(reject("Cargo资源读取期间超限"));
        }
        hash.update(&buffer[..count]);
    }
    if size as u64 != metadata.len() {
        return Err(reject("Cargo资源读取期间改变"));
    }
    Ok((
        hash.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        size,
    ))
}
fn readable(path: &Path, directory: bool) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o755 } else { 0o644 }),
        )
        .map_err(|_| fail("Cargo暂存权限不可设置"))?;
    }
    #[cfg(not(unix))]
    let _ = (path, directory);
    Ok(())
}
fn snapshot_vendor(
    source: &Path,
    target: &Path,
    context: &ToolExecutionContext,
    deadline: Instant,
) -> Result<String> {
    let mut pending = vec![(source.to_path_buf(), PathBuf::new(), 0usize)];
    let mut hashes = BTreeMap::new();
    let mut total = 0usize;
    let mut visited = 0;
    while let Some((directory, relative, depth)) = pending.pop() {
        check_stop(context, deadline)?;
        if depth > 16 {
            return Err(reject("Cargo资源目录过深"));
        }
        for entry in fs::read_dir(directory).map_err(|_| fail("Cargo资源目录不可读"))? {
            check_stop(context, deadline)?;
            visited += 1;
            if visited > 8192 {
                return Err(reject("Cargo资源成员过多"));
            }
            let entry = entry.map_err(|_| fail("Cargo资源目录不可读"))?;
            let relative = relative.join(entry.file_name());
            let name = relative
                .to_str()
                .filter(|s| !s.contains(['\\', ':']) && !s.chars().any(char::is_control))
                .ok_or_else(|| reject("Cargo资源路径非法"))?;
            let metadata =
                fs::symlink_metadata(entry.path()).map_err(|_| fail("Cargo资源不可核对"))?;
            if metadata.is_dir() {
                pending.push((entry.path(), relative, depth + 1));
                continue;
            }
            if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 || hashes.len() >= 4096 {
                return Err(reject("Cargo资源不能是链接且必须有界"));
            }
            let mut bytes = Vec::new();
            fs::File::open(entry.path())
                .map_err(|_| fail("Cargo资源不可读"))?
                .take(8 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| fail("Cargo资源读取失败"))?;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| reject("Cargo资源总量无效"))?;
            if bytes.len() > 8 * 1024 * 1024 || total > 64 * 1024 * 1024 {
                return Err(reject("Cargo资源超过64MiB"));
            }
            hashes.insert(name.to_owned(), crate::archive_digest(&bytes));
            let mut parent = target.to_path_buf();
            for component in relative
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .components()
            {
                parent.push(component);
                fs::create_dir_all(&parent).map_err(|_| fail("Cargo资源暂存失败"))?;
                readable(&parent, true)?;
            }
            fs::write(target.join(relative), bytes).map_err(|_| fail("Cargo资源暂存失败"))?;
        }
    }
    for name in hashes.keys() {
        readable(&target.join(name), false)?;
    }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(b"eve.cargo.vendor.v1\0");
    hash.update(&serde_json::to_vec(&hashes).map_err(|_| fail("Cargo资源清单不可编码"))?);
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

struct Job {
    cli: PathBuf,
    socket: PathBuf,
    name: String,
    armed: bool,
}
impl Job {
    fn command(&self) -> Command {
        client(&self.cli, &self.socket)
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let cli = self.cli.clone();
            let socket = self.socket.clone();
            let name = self.name.clone();
            handle.spawn(async move {
                let mut command = client(&cli, &socket);
                command
                    .args(["rm", "-f", &name])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let _ = tokio::time::timeout(Duration::from_secs(5), command.status()).await;
            });
        }
    }
}
fn client(cli: &Path, socket: &Path) -> Command {
    let mut command = Command::new(cli);
    command.arg(format!("--host=unix://{}", socket.display()));
    for key in [
        "DOCKER_CONTEXT",
        "DOCKER_HOST",
        "DOCKER_TLS",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
    ] {
        command.env_remove(key);
    }
    command.kill_on_drop(true);
    command
}
async fn bounded_output(
    mut command: Command,
    limit: usize,
    context: &ToolExecutionContext,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| fail("Cargo容器命令启动失败"))?;
    let mut bytes = Vec::new();
    guarded(
        async {
            child
                .stdout
                .take()
                .ok_or_else(|| fail("Cargo容器缺少输出"))?
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| fail("Cargo容器输出不可读"))?;
            Ok(())
        },
        context,
        deadline,
    )
    .await?;
    if bytes.len() > limit {
        return Err(reject("Cargo容器输出超限"));
    }
    if !guarded(
        async { child.wait().await.map_err(|_| fail("Cargo容器状态不可读")) },
        context,
        deadline,
    )
    .await?
    .success()
    {
        return Err(fail("Cargo容器命令失败，未降级为宿主执行"));
    }
    Ok(bytes)
}
impl CargoCompiler {
    pub fn bundle_sha256(&self) -> &str {
        &self.config.bundle_sha256
    }
    pub fn new(config: CargoCompilerConfig) -> Result<Self> {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64"))
            || !config.cli.is_absolute()
            || !config.cli.is_file()
            || !config.socket.is_absolute()
            || !config.toolchain.is_absolute()
            || !config.toolchain.is_dir()
            || !config.vendor.is_absolute()
            || !config.vendor.is_dir()
            || !valid_digest(&config.bundle_sha256)
            || !config
                .image
                .strip_prefix("sha256:")
                .is_some_and(valid_digest)
        {
            return Err(reject(
                "Cargo需要Linux x86_64、可信本地CLI/套接字/资源和不可变镜像",
            ));
        }
        Ok(Self { config })
    }
    pub async fn compile(
        &self,
        project: &CargoProject,
        context: ToolExecutionContext,
    ) -> Result<CargoCompilation> {
        project.validate()?;
        if project.bundle_sha256 != self.config.bundle_sha256 {
            return Err(reject("Cargo请求未绑定宿主配置的冻结资源"));
        }
        let owner = context
            .scope()
            .filter(|scope| !scope.user_id.trim().is_empty() && !scope.session_id.trim().is_empty())
            .cloned()
            .ok_or_else(|| reject("Cargo需要真实发起者身份"))?;
        if context.shared_steps().is_none() {
            return Err(reject("Cargo必须继承共享预算"));
        }
        let deadline = context
            .deadline()
            .map(|v| v.min(Instant::now() + Duration::from_secs(30)))
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(30));
        check_stop(&context, deadline)?;
        let _slot = guarded(
            async {
                COMPILE_SLOTS
                    .acquire()
                    .await
                    .map_err(|_| fail("编译准入不可用"))
            },
            &context,
            deadline,
        )
        .await?;
        check_stop(&context, deadline)?;
        reserve(&context)?;
        let mut inspect = client(&self.config.cli, &self.config.socket);
        inspect.args([
            "image",
            "inspect",
            &self.config.image,
            "--format",
            "{\"size\":{{.Size}},\"id\":{{json .Id}}}",
        ]);
        let image: serde_json::Value =
            serde_json::from_slice(&bounded_output(inspect, 1024, &context, deadline).await?)
                .map_err(|_| fail("Cargo镜像元数据无效"))?;
        if image["id"] != self.config.image
            || image["size"].as_u64().is_none_or(|v| v > 64 * 1024 * 1024)
        {
            return Err(reject("Cargo镜像根层超过64MiB或身份不符"));
        }
        let staging = tempfile::tempdir().map_err(|_| fail("Cargo暂存不可创建"))?;
        for path in ["input", "vendor"] {
            fs::create_dir(staging.path().join(path)).map_err(|_| fail("Cargo暂存不可创建"))?;
            readable(&staging.path().join(path), true)?;
        }
        let bundle = snapshot_vendor(
            &self.config.vendor,
            &staging.path().join("vendor"),
            &context,
            deadline,
        )?;
        if bundle != project.bundle_sha256 {
            return Err(reject("Cargo资源与冻结摘要不符"));
        }
        let mut manifest = String::from(
            "[package]\nname=\"eve_plugin\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[dependencies]\n",
        );
        for (name, version) in &project.dependencies {
            manifest.push_str(&format!("{name}=\"={version}\"\n"));
        }
        manifest.push_str(
            "[profile.dev]\ndebug=0\nincremental=false\ncodegen-units=1\npanic=\"abort\"\n",
        );
        for (name, data) in [
            ("input/main.rs", project.source.as_str()),
            ("input/Cargo.toml", manifest.as_str()),
            ("control.py", CONTROL),
        ] {
            let path = staging.path().join(name);
            fs::write(&path, data).map_err(|_| fail("Cargo源码暂存失败"))?;
            readable(&path, false)?;
        }
        if let Some(source) = &project.build_source {
            let path = staging.path().join("input/build.rs");
            fs::write(&path, source).map_err(|_| fail("Cargo构建源码暂存失败"))?;
            readable(&path, false)?;
        }
        let mut nonce = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut nonce)
            .map_err(|_| fail("Cargo随机源不可用"))?;
        let mut job = Job {
            cli: self.config.cli.clone(),
            socket: self.config.socket.clone(),
            name: format!(
                "eve-cargo-{}",
                nonce.iter().map(|v| format!("{v:02x}")).collect::<String>()
            ),
            armed: true,
        };
        let mut command = job.command();
        command.args([
            "run",
            "--rm",
            "--pull=never",
            "--name",
            &job.name,
            "-i",
            "--network=none",
            "--read-only",
            "--user=0:0",
            "--cap-drop=ALL",
            "--cap-add=KILL",
            "--cap-add=CHOWN",
            "--cap-add=FOWNER",
            "--cap-add=SETUID",
            "--cap-add=SETGID",
            "--security-opt=no-new-privileges",
            "--pids-limit=64",
            "--memory=2g",
            "--memory-swap=2g",
            "--cpus=1",
            "--ulimit=nofile=64:64",
            "--ulimit=core=0:0",
        ]);
        for (path, size, mode) in [
            ("tmp", 16, "noexec"),
            ("project", 8, "noexec"),
            ("target", 256, "exec"),
            ("cargo", 16, "noexec"),
            ("out", 16, "noexec"),
        ] {
            command
                .arg("--tmpfs")
                .arg(format!("/{path}:rw,{mode},nosuid,size={size}m"));
        }
        let mut mounts = vec![
            (staging.path().join("input"), "/input"),
            (staging.path().join("vendor"), "/vendor"),
            (staging.path().join("control.py"), "/control.py"),
            (self.config.toolchain.clone(), "/toolchain"),
        ];
        for path in ["/usr/bin", "/usr/lib", "/usr/libexec", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                mounts.push((PathBuf::from(path), path));
            }
        }
        for (host, guest) in mounts {
            let host = host
                .to_str()
                .filter(|s| !s.contains(','))
                .ok_or_else(|| reject("Cargo挂载路径无效"))?;
            command
                .arg("--mount")
                .arg(format!("type=bind,source={host},target={guest},readonly"));
        }
        command.args([
            "--entrypoint=/usr/bin/env",
            &self.config.image,
            "-i",
            "PATH=/usr/bin",
            "HOME=/tmp",
            "/usr/bin/python3",
            "-I",
            "-B",
            "/control.py",
        ]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| fail("Cargo隔离启动失败"))?;
        let mut line = Vec::new();
        let mut output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| fail("Cargo控制器缺少输出"))?,
        );
        guarded(
            async {
                (&mut output)
                    .take(65537)
                    .read_until(b'\n', &mut line)
                    .await
                    .map_err(|_| fail("Cargo控制器输出失败"))?;
                Ok(())
            },
            &context,
            deadline,
        )
        .await?;
        if line.len() > 65536 || !line.ends_with(b"\n") {
            return Err(fail("Cargo控制器没有有界完整回执"));
        }
        let reply: Reply =
            serde_json::from_slice(&line).map_err(|_| fail("Cargo控制器回执无效"))?;
        if reply.diagnostics.len() > 8192
            || reply.code.len() > 64
            || reply.stage.as_ref().is_some_and(|v| v.len() > 64)
            || reply.reason.as_ref().is_some_and(|v| v.len() > 128)
        {
            return Err(reject("Cargo回执字段超限"));
        }
        if !reply.ok {
            drop(child.stdin.take());
            let _ = guarded(
                async { child.wait().await.map_err(|_| fail("Cargo结束状态不可读")) },
                &context,
                deadline,
            )
            .await?;
            job.armed = false;
            return Ok(CargoCompilation::Failed(CargoCompilationFailure {
                code: reply.code,
                stage: reply.stage,
                exit_code: reply.exit_code,
                diagnostics: if reply.diagnostics.is_empty() {
                    reply.reason.unwrap_or_default()
                } else {
                    reply.diagnostics
                },
                truncated: reply.truncated,
            }));
        }
        let limits = reply.limits.ok_or_else(|| fail("Cargo缺少资源证明"))?;
        let memory = limits["memory_bytes"]
            .as_u64()
            .filter(|v| *v > 0 && *v <= 2 * 1024 * 1024 * 1024);
        let pids = limits["pids"].as_u64().filter(|v| *v > 0 && *v <= 64);
        let quota = limits["cpu_quota"].as_u64().filter(|v| *v > 0);
        let period = limits["cpu_period"].as_u64().filter(|v| *v > 0);
        if memory.is_none()
            || pids.is_none()
            || quota.zip(period).is_none_or(|(q, p)| q > p)
            || limits["compiler_uid"].as_u64().is_none_or(|v| v == 0)
            || limits["swap_bytes"].as_u64() != Some(0)
            || limits["disk_available_bytes"]
                .as_u64()
                .is_none_or(|v| v < 512 * 1024 * 1024)
        {
            return Err(reject("Cargo资源证明不符合宿主上限"));
        }
        let sha = reply
            .artifact_sha256
            .filter(|v| valid_digest(v))
            .ok_or_else(|| fail("Cargo产物摘要无效"))?;
        let size = reply
            .artifact_bytes
            .filter(|v| *v > 0 && *v <= ARTIFACT_BYTES)
            .ok_or_else(|| fail("Cargo产物大小无效"))?;
        if reply.code != "compiled" {
            return Err(fail("Cargo回执状态不符"));
        }
        let mut copy = job.command();
        copy.args([
            "exec",
            "--user=0:0",
            &job.name,
            "/usr/bin/cat",
            "/out/plugin",
        ]);
        let bytes = bounded_output(copy, ARTIFACT_BYTES, &context, deadline).await?;
        if bytes.len() != size
            || crate::archive_digest(&bytes) != sha
            || !bytes.starts_with(b"\x7fELF")
        {
            return Err(reject("Cargo交接产物与回执不符"));
        }
        let directory = tempfile::tempdir().map_err(|_| fail("Cargo产物目录不可创建"))?;
        let binary = directory.path().join("plugin");
        fs::write(&binary, bytes).map_err(|_| fail("Cargo产物保存失败"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
                .map_err(|_| fail("Cargo产物权限不可设置"))?;
        }
        drop(child.stdin.take());
        if !guarded(
            async {
                child
                    .wait()
                    .await
                    .map_err(|_| fail("Cargo控制器退出不可确认"))
            },
            &context,
            deadline,
        )
        .await?
        .success()
        {
            return Err(fail("Cargo控制器未正常结束，产物不交付"));
        }
        job.armed = false;
        check_stop(&context, deadline)?;
        Ok(CargoCompilation::Ready(Arc::new(CargoArtifact {
            _directory: directory,
            binary,
            sha256: sha,
            bundle_sha256: bundle,
            owner,
        })))
    }
}
